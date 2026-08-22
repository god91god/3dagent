// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
use std::process::Command;

use serde_json::json;
use tauri::{Emitter, Manager};

mod activity;
mod memory;
mod memory_extract;
mod memory_prompts;
mod memory_reflect;
mod open_threads;
mod proactive;
mod topic;
mod vision;

use activity::{ActivityStateMachine, ActivitySnapshot};
use memory::MemoryStore;
use proactive::ProactiveState;

// 装有 sherpa_onnx 的 Python 绝对路径（WindowsApps 的 python 存根会转发到错误环境）
const PYTHON: &str = r"C:\Users\20728\AppData\Local\Python\pythoncore-3.14-64\python.exe";

/// 全局共享 HTTP 客户端（所有 LLM/视觉调用复用连接池）
/// 历史问题：每处 Client::new() 无连接池复用 + 无超时 → 网络半开时
/// 3 分钟记忆维护循环可永久停摆、聊天永久转圈
static HTTP_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
pub fn shared_http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .connect_timeout(std::time::Duration::from_secs(10))
            // dashscope 国内直连：不走系统代理（防 Clash 等代理干扰/不稳，与 TTS 脚本同款）
            .no_proxy()
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// 长超时 Client（后台重任务用：事实提取/反思/去重仲裁等长 prompt 调用）
/// 共享 Client 60s 对长 prompt 不够（实测 6000 字提取 ~19s，抖动时更久）
pub fn long_timeout_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .connect_timeout(std::time::Duration::from_secs(10))
        .no_proxy()
        .build()
        .map_err(|e| format!("构建 Client 失败: {}", e))
}

/// 聊天模型默认值（config.json 的 chat_model 字段可覆盖，改配置即换模型）
pub const CHAT_MODEL_DEFAULT: &str = "qwen3.6-plus";
/// 视觉模型默认值（config.json 的 vision_model 字段可覆盖）
pub const VISION_MODEL_DEFAULT: &str = "qwen3-vl-plus";

/// 从 config.json 读模型配置（方便随时换模型，不用改代码）
/// key: config.json 字段名；default: 字段缺失/为空时的缺省值
pub fn model_cfg(key: &str, default: &str) -> String {
    let cfg_path = r"D:\3Dagent\3dagent\config.json";
    if let Ok(content) = std::fs::read_to_string(cfg_path) {
        let content = content.trim_start_matches('\u{feff}');
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(content) {
            if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
                if !s.trim().is_empty() {
                    return s.trim().to_string();
                }
            }
        }
    }
    default.to_string()
}

/// 当前聊天模型（对话/事实提取/反思/搭话/话题/activity_guess）
pub fn chat_model() -> String {
    model_cfg("chat_model", CHAT_MODEL_DEFAULT)
}

/// 当前视觉模型（识图/定位/多模态聊天，必须支持看图）
pub fn vision_model() -> String {
    model_cfg("vision_model", VISION_MODEL_DEFAULT)
}

/// 带超时执行子进程：超时则 kill 并返回错误
/// 踩坑记录（两次错误实现）：
///   v1 只 try_wait 不读管道 → 子进程输出 >64KB 阻塞写端永不退出 → 超时
///   v2 循环里直接 read() → ChildStdout::read() 是阻塞读，子进程无输出时
///      永久卡在 read，连超时检查都到不了 → 更糟
///   v3（本版）抄 std::process::Command::output() 的标准做法：用独立线程
///      read_to_end 读管道（永不管道阻塞），主循环只 try_wait + 超时检查
fn run_with_timeout(
    cmd: &mut Command,
    timeout: std::time::Duration,
) -> Result<std::process::Output, String> {
    use std::io::Read;
    use std::process::Stdio;
    let prog = cmd.get_program().to_string_lossy().to_string();
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("启动子进程失败: {}", e))?;
    debug_log(&format!("[subprocess] spawn {} (timeout {:?})", prog, timeout));
    let t0 = std::time::Instant::now();
    // 读管道移到独立线程：std 的 output() 就是这个模式（读线程 + 主循环 wait）
    let mut so = child.stdout.take().ok_or("子进程无 stdout 管道")?;
    let mut se = child.stderr.take().ok_or("子进程无 stderr 管道")?;
    let th_out = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = so.read_to_end(&mut v);
        v
    });
    let th_err = std::thread::spawn(move || {
        let mut v = Vec::new();
        let _ = se.read_to_end(&mut v);
        v
    });
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {}
            Err(e) => return Err(format!("等待子进程失败: {}", e)),
        }
        if std::time::Instant::now() >= deadline {
            debug_log(&format!("[subprocess] TIMEOUT {} killed after {:?}", prog, timeout));
            let _ = child.kill();
            let _ = child.wait();
            // kill 后管道关闭 → 读线程收到 EOF 退出
            let _ = th_out.join();
            let _ = th_err.join();
            return Err(format!("子进程超时（>{:?}），已终止", timeout));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let out = th_out.join().unwrap_or_default();
    let err = th_err.join().unwrap_or_default();
    debug_log(&format!(
        "[subprocess] exit {} ({}ms, stdout {}B, stderr {}B)",
        prog,
        t0.elapsed().as_millis(),
        out.len(),
        err.len()
    ));
    Ok(std::process::Output {
        status,
        stdout: out,
        stderr: err,
    })
}

/// 唯一后缀（毫秒+纳秒）：临时文件名避免并发互相覆盖
fn unique_suffix() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("{}_{}", d.as_millis(), d.subsec_nanos()))
        .unwrap_or_else(|_| format!("{}", std::process::id()))
}

/// 调试日志（写 %TEMP%\dagent_debug.log，排查"未响应"卡点用）
/// 记录线程名：如果同步命令跑在 "main" 线程 → 阻塞窗口消息循环 = 未响应根因
fn debug_log(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("dagent_debug.log"))
    {
        let _ = writeln!(
            f,
            "{} [{}] {}",
            chrono::Local::now().format("%H:%M:%S%.3f"),
            std::thread::current().name().unwrap_or("?"),
            msg
        );
    }
}

/// 语音合成命令：优先 qwen TTS（国内直连稳定），失败自动回退 edge-tts
/// 前端 invoke("tts_speak", { text }) 调用
#[tauri::command]
fn tts_speak(text: String) -> Result<String, String> {
    // 唯一文件名：历史 bug 是固定名（dagent_tts.mp3）——两次并发说话
    // 会互相覆盖文本/音频，播报文字与音频不符
    let out_path = std::env::temp_dir().join(format!("dagent_tts_{}.mp3", unique_suffix()));
    // 文本写临时文件（避免命令行编码问题）
    let text_file = std::env::temp_dir().join(format!("dagent_tts_text_{}.txt", unique_suffix()));
    std::fs::write(&text_file, &text).map_err(|e| format!("写文本文件失败: {}", e))?;

    // 1. 优先 qwen TTS（dashscope 国内直连，不用代理，稳定）
    let qwen_out = run_with_timeout(
        Command::new(PYTHON).args([
            r"D:\3Dagent\tools\tts_core_qwen.py",
            out_path.to_string_lossy().as_ref(),
            text_file.to_string_lossy().as_ref(),
        ]),
        std::time::Duration::from_secs(90),
    )
    .map_err(|e| format!("启动 qwen TTS 失败: {}", e))?;
    if qwen_out.status.success() {
        let _ = std::fs::remove_file(&text_file);
        return Ok(out_path.to_string_lossy().to_string());
    }
    let qwen_err = String::from_utf8_lossy(&qwen_out.stderr).trim().to_string();
    eprintln!("[tts] qwen TTS 失败，回退 edge-tts: {}", qwen_err);

    // 2. 兜底 edge-tts（代理适配 + 超时）
    let output = run_with_timeout(
        Command::new(PYTHON).args([
            r"D:\3Dagent\tools\tts_core.py",
            &text,
            out_path.to_string_lossy().as_ref(),
        ]),
        std::time::Duration::from_secs(90),
    )
    .map_err(|e| format!("启动 edge-tts 失败: {}", e))?;

    let _ = std::fs::remove_file(&text_file);
    if output.status.success() {
        Ok(out_path.to_string_lossy().to_string())
    } else {
        let err = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "TTS 失败: qwen({}) edge-tts({})",
            qwen_err,
            err.trim()
        ))
    }
}

/// 语音识别命令：调 Python sherpa-onnx (SenseVoice) 识别 wav
/// 前端 invoke("asr_transcribe", { wavPath }) 调用
#[tauri::command]
fn asr_transcribe(wav_path: String) -> Result<String, String> {
    // 模型目录（sense-voice 解压后）
    let model_dir = r"D:\3Dagent\model\sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17";
    if !std::path::Path::new(model_dir).join("model.int8.onnx").exists() {
        return Err("ASR 模型不存在，请先下载 sense-voice 模型".to_string());
    }

    let output = run_with_timeout(
        Command::new(PYTHON).args([r"D:\3Dagent\tools\asr_core.py", &wav_path, model_dir]),
        std::time::Duration::from_secs(60),
    )
    .map_err(|e| format!("启动 ASR 失败: {}", e))?;

    if output.status.success() {
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if text.is_empty() {
            Err("识别结果为空".to_string())
        } else {
            Ok(text)
        }
    } else {
        let err = String::from_utf8_lossy(&output.stderr);
        Err(format!("ASR 识别失败: {}", err.trim()))
    }
}

/// 语音识别命令（字节版）：前端传 wav 字节，写临时文件后识别
/// 前端 invoke("asr_transcribe_b64", { data: [byte...] }) 调用
#[tauri::command]
fn asr_transcribe_b64(data: Vec<u8>) -> Result<String, String> {
    let wav_path = std::env::temp_dir().join(format!("dagent_rec_{}.wav", unique_suffix()));
    std::fs::write(&wav_path, &data).map_err(|e| format!("写录音文件失败: {}", e))?;
    let result = asr_transcribe(wav_path.to_string_lossy().to_string());
    let _ = std::fs::remove_file(&wav_path); // 中间文件用完即删
    result
}

// ===== P4-A: Audio2Face 表情驱动（wav2arkit ONNX 推理）=====

/// 音频 → ARKit 52 blendshape 序列
/// 前端 invoke("a2f_blendshapes", { audioPath: mp3路径 }) 调用
/// 返回 JSON: { fps, names: [52], frames: [[52]...] }
#[tauri::command]
fn a2f_blendshapes(audio_path: String) -> Result<String, String> {
    // 1. mp3 → wav 16kHz 单声道（ffmpeg，带超时）
    let wav_path = std::env::temp_dir().join(format!("dagent_a2f_{}.wav", unique_suffix()));
    let ffmpeg_status = run_with_timeout(
        Command::new("ffmpeg")
            .args(["-y", "-i", &audio_path, "-ar", "16000", "-ac", "1"])
            .arg(&wav_path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
        std::time::Duration::from_secs(60),
    )
    .map_err(|e| format!("启动 ffmpeg 失败（请确认已安装）: {}", e))?;
    if !ffmpeg_status.status.success() {
        return Err("ffmpeg 转换音频失败".to_string());
    }

    // 2. Python 推理 → JSON（带超时）
    let output = run_with_timeout(
        Command::new(PYTHON).args([r"D:\3Dagent\tools\a2f_core.py", wav_path.to_string_lossy().as_ref()]),
        std::time::Duration::from_secs(120),
    )
    .map_err(|e| format!("启动 Audio2Face 推理失败: {}", e))?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(format!("Audio2Face 推理失败: {}", err.trim()));
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        return Err("Audio2Face 推理输出为空".to_string());
    }
    // 校验是合法 JSON
    let v: serde_json::Value =
        serde_json::from_str(&stdout).map_err(|e| format!("推理结果解析失败: {}", e))?;
    let _ = std::fs::remove_file(&wav_path); // 中间 wav 用完即删
    Ok(serde_json::to_string(&v).map_err(|e| format!("JSON 序列化失败: {}", e))?)
}

// ===== LLM 对话（DeepSeek，OpenAI 兼容）=====

/// 读取 config.json（自动去 BOM）
fn read_config() -> Result<serde_json::Value, String> {
    let cfg_path = r"D:\3Dagent\3dagent\config.json";
    let content = std::fs::read_to_string(cfg_path)
        .map_err(|_| "未找到 config.json".to_string())?;
    let content = content.trim_start_matches('\u{feff}'); // 去 UTF-8 BOM
    serde_json::from_str(content).map_err(|e| format!("config.json 解析失败: {}", e))
}

/// 从本地 config.json 读千问 API key（意图识别/聊天主力，比 DeepSeek 意图识别更准）
fn get_qwen_api_key() -> Result<String, String> {
    if let Ok(k) = std::env::var("QIANWEN_API_KEY") {
        if !k.is_empty() {
            return Ok(k);
        }
    }
    let cfg = read_config()?;
    cfg["qianwen_api_key"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "config.json 缺少 qianwen_api_key".to_string())
}

/// 对话命令：前端 invoke("llm_chat", { messages, system, jsonMode, imagePath }) 调用
/// 使用千问（通义千问 AI 平台，dashscope 兼容端点），意图识别 + 聊天回复
/// 记忆增强：自动在 system 提示词末尾注入记忆上下文（人格 + 长期事实）
/// imagePath 可选：用户回复附带截图（优香主动搭话时看的屏幕）→ qwen3-vl-plus 多模态
#[tauri::command]
async fn llm_chat(
    messages: Vec<serde_json::Value>,
    system: Option<String>,
    json_mode: Option<bool>,
    image_path: Option<String>,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let key = get_qwen_api_key()?;

    let mut sys = system.unwrap_or_default();
    // 注入记忆上下文（人格 + 长期事实）
    let mem_ctx = state.memory.render_memory_context(8, 2000);
    if !mem_ctx.is_empty() {
        sys.push_str("\n\n");
        sys.push_str(&mem_ctx);
    }

    let mut full_messages: Vec<serde_json::Value> = Vec::new();
    if !sys.trim().is_empty() {
        full_messages.push(serde_json::json!({"role": "system", "content": sys}));
    }
    for m in messages {
        full_messages.push(m);
    }

    // 有截图 → 用户最后一条消息改为多模态（图 + 原文），模型切 qwen3-vl-plus
    let mut model = crate::chat_model();
    if let Some(img_path) = image_path {
        if let Ok(b64) = vision::compress_screenshot_to_b64(std::path::Path::new(&img_path)) {
            // 把最后一条 user 消息转成多模态
            if let Some(last) = full_messages.iter_mut().rev().find(|m| m["role"] == "user") {
                let text = last["content"].as_str().unwrap_or("").to_string();
                *last = serde_json::json!({
                    "role": "user",
                    "content": [
                        {"type": "image_url", "image_url": {"url": format!("data:image/jpeg;base64,{}", b64)}},
                        {"type": "text", "text": text}
                    ]
                });
            }
            model = crate::vision_model();
        }
    }

    let mut body = serde_json::json!({
        "model": model,
        "messages": full_messages,
        "stream": false
    });
    if json_mode.unwrap_or(false) {
        body["response_format"] = serde_json::json!({"type": "json_object"});
    }

    let client = shared_http_client();
    let resp = client
        .post("https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions")
        .bearer_auth(&key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("请求千问失败: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("千问返回 {}: {}", status, text));
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("解析响应失败: {}", e))?;
    body["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "千问响应中没有内容".to_string())
}

/// DSH 办公任务：node 直调 dsh headless（异步，完成后 emit "dsh-task-done"）
/// 前端 invoke("dsh_run", { task }) 调用 → 立即返回 task_id
/// 任务完成/失败 → handle.emit("dsh-task-done", { task_id, ok, output, error })
/// 任务历史写锁（防并发完成任务同时追加导致文件交错）
static TASK_HISTORY_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

/// 读最近 N 条任务历史，拼成上下文前缀注入新任务
/// 背景：DSH headless 每次任务都是全新会话（randomUUID），无对话上下文。
/// 把最近任务+结果拼进任务描述，让 DSH 知道主人最近在搞什么（如"创建桌面快捷方式"）
fn task_history_prefix(history_path: &std::path::Path, max: usize) -> String {
    let mut entries: Vec<(String, bool, String)> = Vec::new();
    if let Ok(content) = std::fs::read_to_string(history_path) {
        for line in content.lines().rev() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                let task = v["task"].as_str().unwrap_or("").to_string();
                let ok = v["ok"].as_bool().unwrap_or(false);
                let result = v["result"].as_str().unwrap_or("").to_string();
                entries.push((task, ok, result));
                if entries.len() >= max {
                    break;
                }
            }
        }
    }
    if entries.is_empty() {
        return String::new();
    }
    let mut out = String::from("【之前的任务记录（供你参考，了解主人最近在做什么）】\n");
    for (i, (task, ok, result)) in entries.iter().rev().enumerate() {
        let status = if *ok { "成功" } else { "失败" };
        let result_short: String = result.chars().take(150).collect();
        out.push_str(&format!("{}. 任务: {} → {}: {}\n", i + 1, task, status, result_short));
    }
    out.push_str("【以上是历史记录，请据此理解上下文，然后执行下面的新任务】\n\n");
    out
}

/// 追加一条任务历史（上限 50 条，防无限增长）
fn append_task_history(history_path: &std::path::Path, task: &str, ok: bool, result: &str) {
    use std::io::Write;
    let _g = TASK_HISTORY_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let entry = serde_json::json!({
        "ts": chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        "task": task.chars().take(200).collect::<String>(),
        "ok": ok,
        "result": result.chars().take(300).collect::<String>(),
    });
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(history_path)
    {
        let _ = writeln!(f, "{}", entry);
    }
    // 裁剪到 50 行（从尾部保留）
    if let Ok(content) = std::fs::read_to_string(history_path) {
        let lines: Vec<&str> = content.lines().collect();
        if lines.len() > 50 {
            let keep = lines[lines.len() - 50..].join("\n");
            let _ = std::fs::write(history_path, format!("{}\n", keep));
        }
    }
}

#[tauri::command]
async fn dsh_run(
    task: String,
    app: tauri::AppHandle,
) -> Result<String, String> {
    let task_id = format!("dsh_{}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0));
    let dsh_js = r"C:\Users\20728\AppData\Roaming\npm\node_modules\@deepseek-ai\dsh\lib\bin.js";
    let out_path = std::env::temp_dir().join(format!("dsh_task_out_{}.txt", task_id));
    let err_path = std::env::temp_dir().join(format!("dsh_task_err_{}.txt", task_id));

    // 任务历史注入（DSH 每次新会话无上下文 → 把最近任务拼进描述）
    let data_dir = app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir().join("dagent"));
    let history_path = data_dir.join("memory").join("dsh_task_history.jsonl");
    let final_task = format!("{}{}", task_history_prefix(&history_path, 10), task);

    // 后台执行，不阻塞前端
    let emit_task_id = task_id.clone();
    let history_path_for_append = history_path.clone();
    let task_for_history = task.clone();
    tauri::async_runtime::spawn(async move {
        let result = run_dsh_headless(&dsh_js, &final_task, &out_path, &err_path).await;
        // 记录任务历史（供下次任务注入上下文）
        match &result {
            Ok(text) => append_task_history(&history_path_for_append, &task_for_history, true, text),
            Err(e) => append_task_history(&history_path_for_append, &task_for_history, false, e),
        }
        // 清理临时文件
        let _ = std::fs::remove_file(&out_path);
        let _ = std::fs::remove_file(&err_path);
        let _ = app.emit("dsh-task-done", serde_json::json!({
            "task_id": emit_task_id,
            "ok": result.is_ok(),
            "output": result.clone().unwrap_or_default(),
            "error": result.err().unwrap_or_default(),
        }));
    });
    Ok(task_id)
}

/// 执行一次 DSH headless 任务（阻塞直到完成）
async fn run_dsh_headless(
    dsh_js: &str,
    task: &str,
    out_path: &std::path::Path,
    err_path: &std::path::Path,
) -> Result<String, String> {
    let mut child = tokio::process::Command::new("node")
        .args([dsh_js, "--profile", "headless", task])
        .stdout(std::fs::File::create(out_path).map_err(|e| format!("创建输出文件失败: {}", e))?)
        .stderr(std::fs::File::create(err_path).map_err(|e| format!("创建错误文件失败: {}", e))?)
        .spawn()
        .map_err(|e| format!("启动 DSH 失败: {}", e))?;

    let status = child
        .wait()
        .await
        .map_err(|e| format!("等待 DSH 失败: {}", e))?;

    if status.success() {
        let text = std::fs::read_to_string(out_path).unwrap_or_default();
        let text = text.trim();
        if text.is_empty() {
            Err("DSH 任务完成但无输出".to_string())
        } else {
            Ok(text.to_string())
        }
    } else {
        let err = std::fs::read_to_string(err_path).unwrap_or_default();
        Err(format!("DSH 任务失败: {}", err.trim()))
    }
}

/// DSH 状态：解析最新会话日志 → 优香感知 DSH 在做什么
/// 前端 invoke("dsh_status") 调用 → { state, percent, currentTask, activity, stage, ... }
#[tauri::command]
fn dsh_status() -> Result<serde_json::Value, String> {
    let py = r"C:\Users\20728\AppData\Local\Python\pythoncore-3.14-64\python.exe";
    let script = r"D:\3Dagent\tools\dsh_status.py";
    let output = run_with_timeout(
        std::process::Command::new(py).arg(script),
        std::time::Duration::from_secs(10),
    )
    .map_err(|e| format!("启动 DSH 状态解析失败: {}", e))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(format!("DSH 状态解析失败: {}", err.trim()));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let v: serde_json::Value =
        serde_json::from_str(stdout.trim()).map_err(|e| format!("DSH 状态 JSON 解析失败: {}", e))?;
    Ok(v)
}

/// Computer-Use Agent（GUI 自动操控，借鉴 N.E.K.O. brain/computer_use.py，Kimi OSWorld 范式）
/// 前端 invoke("computer_use", { task }) 调用 → 立即返回 task_id
/// 异步执行：Python 子进程跑 Thought+Action+Code 循环，完成 emit "computer-use-done"
#[tauri::command]
async fn computer_use(task: String, app: tauri::AppHandle) -> Result<String, String> {
    let task_id = format!("cua_{}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0));
    let py = r"C:\Users\20728\AppData\Local\Python\pythoncore-3.14-64\python.exe";
    let script = r"D:\3Dagent\tools\computer_use.py";
    let emit_task_id = task_id.clone();

    tauri::async_runtime::spawn(async move {
        // 总时长上限：模型生成的代码可能死循环/长 sleep，"停"失效时自动 kill
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10 * 60),
            tokio::process::Command::new(py)
                .args([script, &task])
                .kill_on_drop(true) // 超时 drop 时强制杀子进程（防残留）
                .output(),
        )
        .await;
        let (ok, output, error) = match result {
            Ok(Ok(out)) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
                (true, text, String::new())
            }
            Ok(Ok(out)) => {
                let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
                (false, String::new(), err)
            }
            Ok(Err(e)) => (false, String::new(), format!("启动 CUA 失败: {}", e)),
            Err(_) => (false, String::new(), "CUA 任务超时（10 分钟），已强制终止".to_string()),
        };
        let _ = app.emit("computer-use-done", serde_json::json!({
            "task_id": emit_task_id,
            "ok": ok,
            "output": output,
            "error": error,
        }));
    });
    Ok(task_id)
}

/// 取消正在运行的 Computer-Use 任务：写取消标记文件（Python 端轮询检测）
/// 前端 invoke("computer_use_cancel") 调用
#[tauri::command]
fn computer_use_cancel() -> Result<(), String> {
    let cancel_file = std::env::temp_dir().join("dagent_cua_cancel.txt");
    std::fs::write(&cancel_file, "cancel")
        .map_err(|e| format!("写取消标记失败: {}", e))
}

/// 截屏命令：截取主屏幕 → 保存 PNG → 返回路径
/// 前端 invoke("screenshot") 调用
#[tauri::command]
fn screenshot() -> Result<String, String> {
    let monitors = xcap::Monitor::all().map_err(|e| format!("获取显示器失败: {}", e))?;
    if monitors.is_empty() {
        return Err("没有检测到显示器".to_string());
    }
    let image = monitors[0]
        .capture_image()
        .map_err(|e| format!("截屏失败: {}", e))?;

    let path = std::env::temp_dir().join("dagent_screen.png");
    image
        .save(&path)
        .map_err(|e| format!("保存截图失败: {}", e))?;
    Ok(path.to_string_lossy().to_string())
}

/// 视觉理解命令：截图 + 问题 → qwen3-vl-plus → 描述
/// 前端 invoke("vlm_understand", { imagePath, question }) 调用
#[tauri::command]
async fn vlm_understand(image_path: String, question: String) -> Result<String, String> {
    let key = get_qwen_api_key()?;
    let path = std::path::Path::new(&image_path);
    let b64 = vision::compress_screenshot_to_b64(path)?;
    vision::qwen_vision(
        &key,
        &b64,
        &question,
        "你是优香，一个活泼可爱的二次元AI桌宠助手。你正在观察主人的屏幕。",
        300,
    )
    .await
}

/// 视觉定位：截图 → 缩放 → qwen3-vl-plus 找目标 → 返回全屏坐标
/// 前端 invoke("vlm_locate", { target }) 调用
#[tauri::command]
async fn vlm_locate(target: String) -> Result<(i32, i32), String> {
    // 1. 截图
    let shot_path = std::env::temp_dir().join(format!("dagent_locate_{}.png", unique_suffix()));
    {
        let monitors = xcap::Monitor::all().map_err(|e| format!("获取显示器失败: {}", e))?;
        if monitors.is_empty() {
            return Err("没有检测到显示器".to_string());
        }
        let image = monitors[0]
            .capture_image()
            .map_err(|e| format!("截屏失败: {}", e))?;
        image.save(&shot_path).map_err(|e| format!("保存截图失败: {}", e))?;
    }
    let orig = image::open(&shot_path).map_err(|e| format!("打开截图失败: {}", e))?;
    let (orig_w, orig_h) = (orig.width(), orig.height());

    // 2. qwen3-vl-plus 定位（vision.rs 内部压缩 + 坐标换算）
    let key = get_qwen_api_key()?;
    let result = vision::locate_on_screen(&key, &shot_path, &target, orig_w, orig_h).await;
    let _ = std::fs::remove_file(&shot_path); // 中间截图用完即删
    result
}

/// 鼠标点击：前端 invoke("mouse_click", { x, y }) 调用
#[tauri::command]
fn mouse_click(x: i32, y: i32) -> Result<String, String> {
    use enigo::{Button, Coordinate, Direction, Enigo, Mouse, Settings};
    let mut enigo = Enigo::new(&Settings::default()).map_err(|e| e.to_string())?;
    enigo
        .move_mouse(x, y, Coordinate::Abs)
        .map_err(|e| format!("移动鼠标失败: {}", e))?;
    enigo
        .button(Button::Left, Direction::Click)
        .map_err(|e| format!("点击失败: {}", e))?;
    Ok(format!("已点击 ({}, {})", x, y))
}

/// 键盘输入：前端 invoke("keyboard_type", { text }) 调用
#[tauri::command]
fn keyboard_type(text: String) -> Result<String, String> {
    use enigo::{Enigo, Keyboard, Settings};
    let mut enigo = Enigo::new(&Settings::default()).map_err(|e| e.to_string())?;
    enigo.text(&text).map_err(|e| format!("输入失败: {}", e))?;
    Ok(format!("已输入: {}", text))
}

/// ===== 聊天记录（跨窗口共享，主窗口写入，聊天记录窗口读取）=====
pub struct ChatLog(pub std::sync::Mutex<Vec<serde_json::Value>>);

/// 追加一条聊天记录：前端 invoke("append_chat_log", { role, content }) 调用
#[tauri::command]
fn append_chat_log(
    state: tauri::State<ChatLog>,
    role: String,
    content: String,
) -> Result<(), String> {
    let entry = serde_json::json!({
        "role": role,
        "content": content,
        "ts": chrono::Local::now().format("%H:%M:%S").to_string(),
    });
    let mut log = state
        .0
        .lock()
        .map_err(|e| format!("锁失败: {}", e))?;
    log.push(entry);
    // 上限 500 条：防长会话无限增长（内存泄漏）
    if log.len() > 500 {
        let excess = log.len() - 500;
        log.drain(0..excess);
    }
    Ok(())
}

/// 读取全部聊天记录：前端 invoke("get_chat_log") 调用（聊天记录窗口用）
#[tauri::command]
fn get_chat_log(state: tauri::State<ChatLog>) -> Vec<serde_json::Value> {
    state.0.lock().map(|g| g.clone()).unwrap_or_default()
}

/// 清空聊天记录：前端 invoke("clear_chat_log") 调用
#[tauri::command]
fn clear_chat_log(state: tauri::State<ChatLog>) -> Result<(), String> {
    state.0.lock().map_err(|e| format!("锁失败: {}", e))?.clear();
    Ok(())
}

// ===== 记忆系统（借鉴 N.E.K.O. 五维记忆 L1 子集）=====

/// 应用全局状态：记忆存储 + 活动感知 + 主动搭话 + 话题深池
pub struct AppState {
    pub memory: MemoryStore,
    pub activity: std::sync::Mutex<ActivityStateMachine>,
    pub activity_snapshot: std::sync::Mutex<Option<ActivitySnapshot>>,
    pub proactive: ProactiveState,
    pub topic: topic::TopicPool,
    /// 未收尾话题缓存（语义版追问系统：LLM 检测"提了但没收尾"的话题 → 搭话续聊）
    pub open_threads: open_threads::OpenThreadsCache,
    /// activity_guess 缓存：(签名, 上次成功时间, 叙述文本)
    pub activity_guess_cache: std::sync::Mutex<(String, f64, String)>,
    /// 主动搭话截屏暂存槽：(时间戳, 截图文件路径) —— 用户回复时 llm_chat 注入
    pub proactive_shot: std::sync::Mutex<Option<(f64, String)>>,
}

impl AppState {
    pub fn new(app_data_dir: PathBuf) -> Self {
        AppState {
            memory: MemoryStore::new(app_data_dir.join("memory")),
            activity: std::sync::Mutex::new(ActivityStateMachine::new()),
            activity_snapshot: std::sync::Mutex::new(None),
            proactive: ProactiveState::new(),
            topic: topic::TopicPool::new(app_data_dir.join("memory")),
            open_threads: open_threads::OpenThreadsCache::new(),
            activity_guess_cache: std::sync::Mutex::new((String::new(), 0.0, String::new())),
            proactive_shot: std::sync::Mutex::new(None),
        }
    }
}

use std::path::PathBuf;

/// 追加一轮对话（user 或 assistant）到近期记忆，并后台触发事实提取
/// 前端在每轮对话完成后调用：invoke("memory_append_turn", { role, content })
#[tauri::command]
async fn memory_append_turn(
    role: String,
    content: String,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let store = &state.memory;
    store.append_recent(&role, &content)?;

    // 话题深池信号（用户/AI 轮次都入池，用户轮同时触发回应窗口升级）
    state.topic.note_turn(&role, &content);

    // 未收尾话题（语义版追问系统）：任何一轮新对话都可能留下"没聊完的线头"，
    // 缓存失效（用户消息）→ 后台 LLM 重新检测 → 下次搭话时注入续聊
    if role == "user" {
        state.open_threads.invalidate();
    }

    // 用户回应 → 3-tier 退避归零（搭话频率恢复正常，抄 N.E.K.O. user_input_reset）
    if role == "user" {
        state.proactive.reset_backoff();
    }

    // 后台触发事实提取（fire-and-forget，不阻塞前端）
    // 只在 user 轮触发，避免提取空轮
    if role == "user" && content.trim().len() >= 8 {
        let recent_text = store.get_recent_conversation_text(6000);
        let store_dir = store.dir();
        let key = get_qwen_api_key().unwrap_or_default();
        if !key.is_empty() && !recent_text.is_empty() {
            tauri::async_runtime::spawn(async move {
                let store = MemoryStore::new(store_dir);
                match crate::memory_extract::extract_facts(
                    &store,
                    &recent_text,
                    &key,
                    "优香",
                    "主人",
                )
                .await
                {
                    Ok((added, dup)) => {
                        eprintln!("[memory] 事实提取完成: 新增 {} 条, 重复 {} 条", added, dup);
                    }
                    Err(e) => {
                        eprintln!("[memory] 事实提取失败: {}", e);
                    }
                }
            });
        }
    }
    Ok("ok".to_string())
}

/// 记忆召回：invoke("memory_recall", { query, maxItems })
#[tauri::command]
fn memory_recall(
    query: String,
    max_items: Option<usize>,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let max = max_items.unwrap_or(6).min(10);
    Ok(state.memory.recall(&query, max))
}

/// 记忆全量概览：invoke("memory_summary") → 完整画像
/// 用户问"关于我你知道些什么/你了解我多少"时用（比日常注入更全：30 条事实 + 反思 + 人格）
#[tauri::command]
fn memory_summary(state: tauri::State<'_, AppState>) -> Result<String, String> {
    let ctx = state.memory.render_memory_context(30, 5000);
    if ctx.trim().is_empty() {
        return Err("记忆还是空的，多和我聊聊我就能记住你啦".to_string());
    }
    Ok(ctx)
}

/// 读取记忆概览：invoke("memory_get") → { facts: [...], recent: [...], persona: {...} }
#[tauri::command]
fn memory_get(state: tauri::State<'_, AppState>) -> Result<serde_json::Value, String> {
    let facts: Vec<serde_json::Value> = state
        .memory
        .get_facts()
        .into_iter()
        .map(|f| {
            json!({
                "id": f.id,
                "text": f.text,
                "importance": f.importance,
                "entity": f.entity,
                "created_at": f.created_at,
            })
        })
        .collect();
    let persona = state.memory.get_persona();
    let recent = state.memory.get_recent();
    Ok(json!({
        "facts": facts,
        "recent": recent,
        "persona": persona,
    }))
}

/// 手动添加人格设定：invoke("memory_add_persona", { entity, text })
#[tauri::command]
fn memory_add_persona(
    entity: String,
    text: String,
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    let entity = match entity.as_str() {
        "master" | "neko" | "relationship" => entity,
        _ => return Err("entity 必须是 master/neko/relationship".to_string()),
    };
    match state.memory.add_persona_fact(&entity, &text)? {
        true => Ok("已记住".to_string()),
        false => Ok("已经记住了".to_string()),
    }
}

// ===== 办公能力（全部交给 DSH）=====
// 文件查找/读取/删除/移动/重命名等一律走 dsh_run（headless agent），
// 不再本地实现 file_ops —— 避免"一会千问一会 dsh"的双轨混乱。
// 状态感知走 dsh_status（解析 DSH 会话日志）。

/// 读取活动快照：invoke("activity_snapshot") → { state, label, propensity, window, idle, ... }
#[tauri::command]
fn activity_snapshot(state: tauri::State<'_, AppState>) -> Result<serde_json::Value, String> {
    let snap = state
        .activity_snapshot
        .lock()
        .map_err(|e| e.to_string())?
        .clone()
        .ok_or_else(|| "活动感知尚未启动".to_string())?;
    Ok(json!({
        "state": snap.state.label(),
        "state_key": format!("{:?}", snap.state),
        "state_age_seconds": snap.state_age_seconds,
        "propensity": snap.propensity.label(),
        "skip_probability": snap.skip_probability,
        "tone": snap.tone,
        "window": snap.active_window,
        "idle_seconds": snap.idle_seconds,
        "switch_rate_5min": snap.switch_rate_5min,
        "hour": snap.hour,
        "activity_guess": snap.activity_guess,
    }))
}

/// 取主动搭话的暂存截图（用户回复时注入）：invoke("proactive_shot_take")
/// 返回 Some(路径) 且消费清空；超时（60s）或无截图 → None
#[tauri::command]
fn proactive_shot_take(state: tauri::State<'_, AppState>) -> Option<String> {
    let mut slot = state.proactive_shot.lock().ok()?;
    let (ts, path) = slot.take()?;
    // 60s TTL（抄 N.E.K.O. _PROACTIVE_SCREENSHOT_TTL_SECONDS）
    if now_secs_fn() - ts > 60.0 {
        return None;
    }
    if !std::path::Path::new(&path).exists() {
        return None;
    }
    Some(path)
}

/// 当前 unix 秒（f64）
fn now_secs_fn() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            let data_dir = app.path().app_data_dir().unwrap_or_else(|_| {
                std::env::temp_dir().join("dagent")
            });
            let state = AppState::new(data_dir);
            app.manage(state);

            // 活动感知轮询（L2-B）+ 主动搭话（L3）：每 5s 采集 → 状态机 → 提醒累积 → 搭话生成
            let activity_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                // 搭话生成节流：提醒/搭话检测每 20s 一次（LLM 调用不频繁）
                let mut check_tick = 0u32;
                let mut last_hb = std::time::Instant::now();
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
                loop {
                    interval.tick().await;
                    check_tick += 1;
                    // 心跳：每 60s 写 debug.log（终端管道可能被 Vite watcher 崩溃弄断，
                    // debug.log 绕开终端确认后台循环死活）
                    if last_hb.elapsed().as_secs() >= 60 {
                        last_hb = std::time::Instant::now();
                        debug_log(&format!("[activity] 心跳 tick={}（后台循环存活）", check_tick));
                    }
                    let signals = activity::collect_system_signals();
                    let Some(state) = activity_handle.try_state::<AppState>() else {
                        continue;
                    };
                    // 1. 更新状态机 + 缓存快照
                    let mut reminder: Option<proactive::ReminderKind> = None;
                    let mut snapshot: Option<ActivitySnapshot> = None;
                    if let Ok(mut sm) = state.activity.lock() {
                        sm.update_signals(&signals);
                        let snap = sm.get_snapshot();
                        // 2. 提醒累积器（每个 tick 都跑）
                        reminder = state.proactive.tick(&snap);
                        snapshot = Some(snap.clone());
                        if let Ok(mut cache) = state.activity_snapshot.lock() {
                            *cache = Some(snap);
                        }
                    }
                    // 3. activity_guess：签名变化时后台 LLM 叙述"主人在干嘛"（喂给搭话 prompt）
                    //    退避：签名没变就不重算（抄 N.E.K.O. ActivityGuessGate 粗粒度签名）
                    {
                        // 锁全部用 unwrap_or_else(|e| e.into_inner())：防锁中毒 panic 杀死后台循环
                        let sig = {
                            let sm = state.activity.lock().unwrap_or_else(|e| e.into_inner());
                            sm.guess_signature()
                        };
                        let mut cache = state.activity_guess_cache.lock().unwrap_or_else(|e| e.into_inner());
                        let (cached_sig, cached_at, _) = cache.clone();
                        // 签名变化 → 立即重算；同签名 → 5min 冷却
                        let now_secs = now_secs_fn();
                        let need_refresh = cached_sig != sig
                            || cached_sig.is_empty()
                            || (now_secs - cached_at > 300.0 && !sig.is_empty());
                        if need_refresh {
                            *cache = (sig.clone(), now_secs, cached_sig.clone()); // 占位防并发重复
                            let handle_for_guess = activity_handle.clone();
                            tauri::async_runtime::spawn(async move {
                                let key = match get_qwen_api_key() {
                                    Ok(k) => k,
                                    Err(_) => return,
                                };
                                // 快照信号 + 最近对话
                                let signals_text = {
                                    let Some(state) = handle_for_guess.try_state::<AppState>() else { return };
                                    let cache = state.activity_snapshot.lock().unwrap_or_else(|e| e.into_inner());
                                    cache.as_ref().map(|s| {
                                        format!(
                                            "窗口: {} | 空闲: {}s | 状态持续: {}s | 5min切换: {}次 | 状态: {:?}",
                                            s.active_window, s.idle_seconds, s.state_age_seconds as i64, s.switch_rate_5min, s.state
                                        )
                                    }).unwrap_or_default()
                                };
                                let conversation = {
                                    let dir = handle_for_guess.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir().join("dagent"));
                                    let store = MemoryStore::new(dir.join("memory"));
                                    store.get_recent_conversation_text(1500)
                                };
                                let rule_state = {
                                    let Some(state) = handle_for_guess.try_state::<AppState>() else { return };
                                    let cache = state.activity_snapshot.lock().unwrap_or_else(|e| e.into_inner());
                                    cache.as_ref().map(|s| s.state.label().to_string()).unwrap_or_default()
                                };
                                match vision::call_activity_guess(&key, &signals_text, &conversation, &rule_state).await {
                                    Ok(Some(guess)) => {
                                        if let Some(state) = handle_for_guess.try_state::<AppState>() {
                                            // 更新缓存 + 快照里的叙述
                                            let mut cache = state.activity_guess_cache.lock().unwrap_or_else(|e| e.into_inner());
                                            *cache = (sig.clone(), now_secs_fn(), guess.guess.clone());
                                            if let Ok(mut snap) = state.activity_snapshot.lock() {
                                                if let Some(s) = snap.as_mut() {
                                                    s.activity_guess = guess.guess.clone();
                                                    s.guess_signature = sig.clone();
                                                }
                                            }
                                        }
                                        debug_log(&format!("[activity_guess] {}", guess.guess));
                                    }
                                    Ok(None) => {
                                        if let Some(state) = handle_for_guess.try_state::<AppState>() {
                                            let mut cache = state.activity_guess_cache.lock().unwrap_or_else(|e| e.into_inner());
                                            cache.1 = 0.0; // 失败 → 下次立刻重试
                                        }
                                    }
                                    Err(e) => {
                                        debug_log(&format!("[activity_guess] 失败: {}", e));
                                    }
                                }
                            });
                        }
                    }
                    // 4. 每 20s（4 tick）尝试一次搭话/提醒生成
                    if check_tick % 4 != 0 {
                        continue;
                    }
                    let Some(snap) = snapshot else { continue };
                    // 有 must-fire 提醒 → 直接生成（跳过普通冷却）
                    let is_reminder = reminder.is_some();
                    let should_attempt = is_reminder || state.proactive.should_speak(&snap, signals.idle_seconds);
                    if !should_attempt {
                        continue;
                    }
                    // 话题深池：每次 20s tick 尝试后台分析（信号 ready + 成熟 + 冷却内才真正调 LLM）
                    let pool_for_analysis = activity_handle.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir().join("dagent")).join("memory");
                    tauri::async_runtime::spawn(async move {
                        let key = match get_qwen_api_key() {
                            Ok(k) => k,
                            Err(_) => return,
                        };
                        let pool = topic::TopicPool::new(pool_for_analysis);
                        match pool.process_now(&key).await {
                            Ok(true) => eprintln!("[topic] 话题分析: 新增深话题素材"),
                            Ok(false) => {}
                            Err(e) => eprintln!("[topic] 话题分析失败: {}", e),
                        }
                    });
                    // 生成（LLM 调用放后台，不阻塞轮询）
                    let handle = activity_handle.clone();
                    let snap_for_gen = snap.clone();
                    let kind = reminder.take();
                    let is_reminder = is_reminder;
                    // 3-tier 退避语气层：级别 ≥1 时生成克制文案（主人多次没回应）
                    let restrained = state.proactive.current_backoff_level() >= 1;
                    let store = MemoryStore::new(handle.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir().join("dagent")).join("memory"));
                    tauri::async_runtime::spawn(async move {
                        let key = match get_qwen_api_key() {
                            Ok(k) => k,
                            Err(_) => return,
                        };
                        // 未收尾话题检测（语义版追问系统，抄 N.E.K.O. kickoff_open_threads_compute）：
                        // 用户发过消息且冷却过 → 懒计算一次 → 写入共享缓存 → 注入搭话 prompt
                        // 让优香自然续上"提了但没收尾"的话题（"你刚说那个 bug 后来呢？"）
                        let open_threads = {
                            let should_detect = handle
                                .try_state::<AppState>()
                                .map(|s| s.open_threads.should_recompute())
                                .unwrap_or(false);
                            if should_detect {
                                let recent = store.get_recent_conversation_text(3000);
                                if !recent.trim().is_empty() {
                                    match open_threads::detect_open_threads(&key, &recent).await {
                                        Ok(threads) => {
                                            if let Some(state) = handle.try_state::<AppState>() {
                                                state.open_threads.store(threads.clone());
                                            }
                                            if !threads.is_empty() {
                                                debug_log(&format!(
                                                    "[open_threads] 检测到 {} 条未收尾话题",
                                                    threads.len()
                                                ));
                                            }
                                            threads
                                        }
                                        Err(e) => {
                                            eprintln!("[open_threads] 检测失败: {}", e);
                                            Vec::new()
                                        }
                                    }
                                } else {
                                    Vec::new()
                                }
                            } else {
                                handle
                                    .try_state::<AppState>()
                                    .map(|s| s.open_threads.current())
                                    .unwrap_or_default()
                            }
                        };
                        let text = if is_reminder {
                            // must-fire 提醒最优先：直接走原搭话流程，话题投递让路
                            match proactive::generate_proactive(
                                &store,
                                &key,
                                &snap_for_gen,
                                kind.as_ref(),
                                "主人",
                                None,
                                restrained,
                                &open_threads,
                            )
                            .await
                            {
                                Ok(Some(t)) => t,
                                Ok(None) => return, // [PASS]
                                Err(e) => {
                                    eprintln!("[proactive] 生成失败: {}", e);
                                    return;
                                }
                            }
                        } else {
                            // 搭话前截图（抄 N.E.K.O. Phase 2 request_fresh_screenshot）：
                            // 让 vision 模型直接看屏幕生成搭话，并暂存给用户回复注入
                            let shot_path = std::env::temp_dir().join(format!("dagent_proactive_shot_{}.png", unique_suffix()));
                            let screen_b64 = {
                                let shot_ok = (|| -> Result<String, String> {
                                    let monitors = xcap::Monitor::all().map_err(|e| format!("获取显示器失败: {}", e))?;
                                    if monitors.is_empty() {
                                        return Err("没有检测到显示器".to_string());
                                    }
                                    let image = monitors[0]
                                        .capture_image()
                                        .map_err(|e| format!("截屏失败: {}", e))?;
                                    image.save(&shot_path).map_err(|e| format!("保存截图失败: {}", e))?;
                                    vision::compress_screenshot_to_b64(&shot_path)
                                })();
                                match shot_ok {
                                    Ok(b64) => {
                                        // 暂存给用户回复（llm_chat 注入 leading image）
                                        if let Some(state) = handle.try_state::<AppState>() {
                                            if let Ok(mut slot) = state.proactive_shot.lock() {
                                                *slot = Some((now_secs_fn(), shot_path.to_string_lossy().to_string()));
                                            }
                                        }
                                        Some(b64)
                                    }
                                    Err(e) => {
                                        eprintln!("[proactive] 搭话截图失败: {}", e);
                                        None
                                    }
                                }
                            };
                            // 话题深池投递：有 ready 素材 → 生成话题开场
                            let topic_pool = topic::TopicPool::new(handle.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir().join("dagent")).join("memory"));
                            if let Some(mat) = topic_pool.next_ready_material() {
                                let mem_ctx = store.render_memory_context(5, 800);
                                let activity_section = crate::activity::format_activity_state_section(&snap_for_gen);
                                match topic::generate_topic_opening(&key, &mat, &mem_ctx, &activity_section).await {
                                    Ok(Some(t)) => {
                                        // 投递成功记账（weight 1/3 + 回应窗口）
                                        topic_pool.mark_topic_used(&mat);
                                        eprintln!("[topic] 话题投递: {}", t);
                                        t
                                    }
                                    Ok(None) => return, // [PASS] 保留素材等下次
                                    Err(e) => {
                                        eprintln!("[topic] 话题开场生成失败: {}", e);
                                        return;
                                    }
                                }
                            } else {
                                // 无提醒无话题素材 → 普通搭话（vision 看屏幕生成）
                                match proactive::generate_proactive(
                                    &store,
                                    &key,
                                    &snap_for_gen,
                                    None,
                                    "主人",
                                    screen_b64.as_deref(),
                                    restrained,
                                    &open_threads,
                                )
                                .await
                                {
                                    Ok(Some(t)) => t,
                                    Ok(None) => return, // [PASS]
                                    Err(e) => {
                                        eprintln!("[proactive] 生成失败: {}", e);
                                        return;
                                    }
                                }
                            }
                        };
                        // 防复读
                        if let Some(state) = handle.try_state::<AppState>() {
                            if state.proactive.is_repeat(&text) {
                                debug_log(&format!("[proactive] 防复读拦截: {}", text));
                                return;
                            }
                            state.proactive.record_delivery(&text);
                        }
                        debug_log(&format!("[proactive] 搭话: {}", text));
                        let _ = handle.emit("proactive-nudge", serde_json::json!({ "text": text }));
                    });
                }
            });

            // 记忆后台维护循环（L2 证据闭环）：每 3 分钟
            // 反思合成 → 信号检测 → 晋升扫描
            let mem_dir = app.path().app_data_dir().unwrap_or_else(|_| {
                std::env::temp_dir().join("dagent")
            });
            tauri::async_runtime::spawn(async move {
                let store = MemoryStore::new(mem_dir.join("memory"));
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(180));
                loop {
                    interval.tick().await;
                    let key = match get_qwen_api_key() {
                        Ok(k) => k,
                        Err(_) => continue,
                    };
                    // 1. 反思合成
                    match crate::memory_reflect::synthesize_reflection(&store, &key, "优香", "主人").await {
                        Ok(true) => eprintln!("[memory] 反思合成: 新增 1 条"),
                        Ok(false) => {}
                        Err(e) => eprintln!("[memory] 反思合成失败: {}", e),
                    }
                    // 2. 信号检测（新事实 × 已有反思）
                    match crate::memory_reflect::detect_signals(&store, &key).await {
                        Ok((applied, skipped)) => {
                            if applied > 0 {
                                eprintln!("[memory] 信号检测: 应用 {} 条, 跳过 {} 条", applied, skipped);
                            }
                        }
                        Err(e) => eprintln!("[memory] 信号检测失败: {}", e),
                    }
                    // 3. 晋升扫描
                    let (c, p) = store.promote_reflections();
                    if c > 0 || p > 0 {
                        eprintln!("[memory] 晋升: confirmed {} 条, promoted {} 条", c, p);
                    }
                    // 4. 事实去重仲裁（抄 N.E.K.O. FactDedupResolver：LLM 判 merge/replace/keep_both）
                    //    历史问题：相似事实（"主人叫小李"×5 个变体）堆积，注入被重复信息占满
                    match store.resolve_fact_dedup(&key).await {
                        Ok((m, r, k)) => {
                            if m > 0 || r > 0 {
                                eprintln!("[memory] 去重仲裁: merge {} 条, replace {} 条, keep {} 对", m, r, k);
                            }
                        }
                        Err(e) => eprintln!("[memory] 去重仲裁失败: {}", e),
                    }
                }
            });
            Ok(())
        })
        .manage(ChatLog(std::sync::Mutex::new(Vec::new())))
        .invoke_handler(tauri::generate_handler![
            tts_speak,
            asr_transcribe,
            asr_transcribe_b64,
            a2f_blendshapes,
            llm_chat,
            dsh_run,
            dsh_status,
            computer_use,
            computer_use_cancel,
            screenshot,
            vlm_understand,
            vlm_locate,
            mouse_click,
            keyboard_type,
            append_chat_log,
            get_chat_log,
            clear_chat_log,
            memory_append_turn,
            memory_recall,
            memory_summary,
            memory_get,
            memory_add_persona,
            activity_snapshot,
            proactive_shot_take,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
