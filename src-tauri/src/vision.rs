// ============================================================
// 视觉能力统一层 —— 借鉴 N.E.K.O. main_routers/system_router/screenshot.py
// 源文件: N.E.K.O-main/main_routers/system_router/screenshot.py
//         N.E.K.O-main/main_logic/activity/llm_enrichment.py
// 原作者: Project N.E.K.O. Team (Copyright 2025-2026)
// 许可: Apache License 2.0 —— 本文件保留上游版权声明
//
// 统一视觉模型：qwen3-vl-plus（与 DSH vision-router 同款）
//   - 截图压缩：720p/JPEG（避免大图 413 / 拖慢）
//   - describe_screen：被动识图（"看看屏幕"）
//   - proactive_screen：主动搭话的屏幕段（vision 模型直接看截图生成）
//   - activity_guess：快照信号 + 对话 → 软评分 + 一句话叙述（喂给搭话 prompt）
// ============================================================
#![allow(dead_code)]

use base64::Engine;
use serde_json::{json, Value};
use std::path::Path;

/// 截图压缩目标高度（抄 N.E.K.O. COMPRESS_TARGET_HEIGHT）
pub const COMPRESS_TARGET_HEIGHT: u32 = 720;
/// JPEG 质量（抄 N.E.K.O. COMPRESS_JPEG_QUALITY）
pub const JPEG_QUALITY: u8 = 80;

/// 读本地截图 → 压缩到 720p/JPEG → 返回 base64 data URL（不含前缀，调用方按需加）
pub fn compress_screenshot_to_b64(path: &Path) -> Result<String, String> {
    let img = image::open(path).map_err(|e| format!("打开截图失败: {}", e))?;
    // 按比例缩到目标高度（抄 N.E.K.O. compress_screenshot 的高度对齐）
    let (w, h) = (img.width(), img.height());
    let (nw, nh) = if h > COMPRESS_TARGET_HEIGHT {
        let scale = COMPRESS_TARGET_HEIGHT as f64 / h as f64;
        (((w as f64 * scale) as u32).max(1), COMPRESS_TARGET_HEIGHT)
    } else {
        (w, h)
    };
    let small = img.resize(nw, nh, image::imageops::FilterType::Triangle);
    let mut buf = std::io::Cursor::new(Vec::new());
    small
        .write_to(&mut buf, image::ImageFormat::Jpeg)
        .map_err(|e| format!("JPEG 编码失败: {}", e))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(buf.into_inner()))
}

/// 截图路径 → data URL（"data:image/jpeg;base64,..."）
pub fn screenshot_data_url(path: &Path) -> Result<String, String> {
    compress_screenshot_to_b64(path).map(|b| format!("data:image/jpeg;base64,{}", b))
}

/// qwen3-vl-plus 多模态调用：一张图 + 文本 prompt → 文本回答
/// image_b64：JPEG base64（不带前缀）
pub async fn qwen_vision(
    api_key: &str,
    image_b64: &str,
    prompt: &str,
    system: &str,
    max_tokens: u32,
) -> Result<String, String> {
    let body = json!({
        "model": crate::vision_model(),
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": format!("data:image/jpeg;base64,{}", image_b64)}},
                {"type": "text", "text": prompt}
            ]}
        ],
        "stream": false,
        "max_tokens": max_tokens
    });

    let client = crate::shared_http_client();
    let resp = client
        .post("https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions")
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("请求 qwen-vl 失败: {}", e))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("qwen-vl 接口返回 {}: {}", status, text));
    }
    let resp_body: Value = resp
        .json()
        .await
        .map_err(|e| format!("解析 qwen-vl 响应失败: {}", e))?;
    resp_body["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.trim().to_string())
        .ok_or_else(|| "qwen-vl 响应无内容".to_string())
}

/// 被动识图 prompt（"看看屏幕"）
pub const DESCRIBE_SCREEN_PROMPT: &str =
    "请用一两句话简要描述屏幕上现在显示的内容，包括主要窗口/应用、正在进行的任务，中文回答。";

/// 被动识图：截图路径 → 屏幕描述
pub async fn describe_screen(api_key: &str, shot_path: &Path) -> Result<String, String> {
    let b64 = compress_screenshot_to_b64(shot_path)?;
    qwen_vision(
        api_key,
        &b64,
        DESCRIBE_SCREEN_PROMPT,
        "你是优香，一个活泼可爱的二次元AI桌宠助手。你正在观察主人的屏幕。",
        300,
    )
    .await
}

/// 视觉定位：截图路径 + 目标 → 全屏坐标（qwen3-vl-plus）
pub async fn locate_on_screen(
    api_key: &str,
    shot_path: &Path,
    target: &str,
    orig_w: u32,
    orig_h: u32,
) -> Result<(i32, i32), String> {
    let b64 = compress_screenshot_to_b64(shot_path)?;
    // 压缩后实际尺寸由 VLM 看到的图决定：固定 720 高，宽按原比例
    let scale = orig_h as f64 / COMPRESS_TARGET_HEIGHT as f64;
    let small_w = ((orig_w as f64) / scale) as u32;
    let prompt = format!(
        "在图片中寻找目标：{}。如果找到，输出其中心点的像素坐标，格式严格为：x,y（例如 640,480）。图片宽度 {} 像素，高度 {} 像素。如果找不到，输出：notfound",
        target, small_w, COMPRESS_TARGET_HEIGHT
    );
    let answer = qwen_vision(
        api_key,
        &b64,
        &prompt,
        "你是屏幕定位助手，只输出坐标。",
        100,
    )
    .await?;

    // 解析坐标（手动扫描数字对，兼容 "640,480" / "x=100 y=200" 等格式）
    let mut nums: Vec<i32> = Vec::new();
    let mut cur = String::new();
    for c in answer.chars() {
        if c.is_ascii_digit() || c == '-' {
            cur.push(c);
        } else if !cur.is_empty() {
            if let Ok(n) = cur.parse::<i32>() {
                nums.push(n);
            }
            cur.clear();
        }
    }
    if !cur.is_empty() {
        if let Ok(n) = cur.parse::<i32>() {
            nums.push(n);
        }
    }
    if answer.to_lowercase().contains("notfound") || nums.len() < 2 {
        return Err(format!("屏幕上没找到: {}（模型回答: {}）", target, answer.trim()));
    }
    let (sx, sy) = (nums[0] as f64, nums[1] as f64);
    let fx = (sx * scale).round() as i32;
    let fy = (sy * scale).round() as i32;
    Ok((fx, fy))
}

// ── activity_guess：快照信号 + 对话 → 软评分 + 一句话叙述 ──

/// activity_guess prompt（抄 prompts_activity.py ACTIVITY_GUESS_PROMPTS["zh"]）
pub const ACTIVITY_GUESS_PROMPT: &str = r#"你是一个用户活动分析助手。基于下方的系统信号和最近对话片段，对用户当前的活动状态做软评分，并写一句简短的活动叙述。

======以下为系统信号======
{signals}
======以上为系统信号======

======以下为最近对话（按时间顺序）======
{conversation}
======以上为最近对话（按时间顺序）======

======以下为规则系统的初判======
{rule_state}
======以上为规则系统的初判======

请输出严格的 JSON（不带 markdown 代码块），字段：
- "scores": 一个对象，键是状态名，值是 0.0-1.0 的浮点数（独立打分，不需要归一化）。允许的状态名：{state_keys}
- "guess": 一句话叙述用户当前在做什么，符合中文表达习惯，不超过 40 字

如果某状态完全不像，给 0.0；如果非常像，给接近 1.0。多个状态可以同时高分（例如同时在写代码和聊天）。

如果你的判断和"规则系统的初判"不同，按你看到的实际信号给分；规则只是参考，不必盲从。

输出示例：
{{"scores": {{"focused_work": 0.7, "chatting": 0.2, "idle": 0.1, "gaming": 0.0, "casual_browsing": 0.0, "voice_engaged": 0.0}}, "guess": "用户在 VS Code 里写代码，偶尔切到聊天软件回消息"}}"#;

/// 可评分的状态名（抄 N.E.K.O. _SCORED_STATES，跳过纯规则派生的）
pub const SCORED_STATES: &[&str] = &[
    "gaming",
    "focused_work",
    "casual_browsing",
    "focused_video",
    "chatting",
    "voice_engaged",
    "idle",
];

/// activity_guess 结果
#[derive(Debug, Clone, Default)]
pub struct ActivityGuess {
    pub scores: std::collections::HashMap<String, f64>,
    pub guess: String,
}

/// 调用 LLM 做 activity_guess（crate::CHAT_MODEL 文本模型，抄 N.E.K.O. call_activity_guess）
/// 返回 None = 失败（保留旧缓存）
pub async fn call_activity_guess(
    api_key: &str,
    signals_text: &str,
    conversation_text: &str,
    rule_state: &str,
) -> Result<Option<ActivityGuess>, String> {
    let prompt = ACTIVITY_GUESS_PROMPT
        .replace("{signals}", signals_text)
        .replace("{conversation}", if conversation_text.is_empty() { "(暂无对话)" } else { conversation_text })
        .replace("{rule_state}", rule_state)
        .replace("{state_keys}", &SCORED_STATES.join(", "));

    let body = json!({
        "model": crate::chat_model(),
        "messages": [
            {"role": "system", "content": "你是活动分析助手，只输出 JSON。"},
            {"role": "user", "content": prompt}
        ],
        "stream": false,
        "max_tokens": 300,
        "response_format": {"type": "json_object"}
    });

    let client = crate::shared_http_client();
    let resp = client
        .post("https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions")
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("activity_guess 请求失败: {}", e))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("activity_guess 接口返回 {}: {}", status, text));
    }
    let resp_body: Value = resp
        .json()
        .await
        .map_err(|e| format!("activity_guess 响应解析失败: {}", e))?;
    let content = resp_body["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| "activity_guess 响应无内容".to_string())?;

    let parsed = parse_guess_json(content);
    Ok(parsed)
}

fn parse_guess_json(raw: &str) -> Option<ActivityGuess> {
    let mut text = raw.trim().to_string();
    if let Some(stripped) = strip_json_fence(&text) {
        text = stripped;
    }
    let v: Value = serde_json::from_str(&text).ok()?;
    if !v.is_object() {
        return None;
    }
    let mut scores = std::collections::HashMap::new();
    if let Some(obj) = v.get("scores").and_then(|x| x.as_object()) {
        for key in SCORED_STATES {
            if let Some(val) = obj.get(*key).and_then(|x| x.as_f64()) {
                scores.insert(key.to_string(), val.clamp(0.0, 1.0));
            }
        }
    }
    let guess = v.get("guess").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
    Some(ActivityGuess { scores, guess })
}

fn strip_json_fence(text: &str) -> Option<String> {
    let t = text.trim();
    let start = t.find("```")?;
    let after = &t[start + 3..];
    let body_start = after.find('\n').map(|i| i + 1).unwrap_or(0);
    let body = &after[body_start..];
    let end = body.rfind("```")?;
    Some(body[..end].trim().to_string())
}

// ============================================================
// 单元测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_guess_json() {
        let raw = r#"{"scores": {"focused_work": 0.7, "chatting": 0.2, "gaming": 0.0, "hack": 9.9}, "guess": "用户在 VS Code 里写代码"}"#;
        let g = parse_guess_json(raw).expect("应解析成功");
        assert_eq!(g.scores.get("focused_work"), Some(&0.7));
        assert_eq!(g.scores.get("chatting"), Some(&0.2));
        assert!(!g.scores.contains_key("hack"), "非法状态名应被过滤");
        assert_eq!(g.guess, "用户在 VS Code 里写代码");
    }

    #[test]
    fn test_parse_guess_json_fence() {
        let raw = "```json\n{\"scores\": {\"idle\": 1.0}, \"guess\": \"用户在发呆\"}\n```";
        let g = parse_guess_json(raw).expect("应解析成功");
        assert_eq!(g.scores.get("idle"), Some(&1.0));
    }

    #[test]
    fn test_parse_guess_json_bad() {
        assert!(parse_guess_json("不是 JSON").is_none());
        // 合法 JSON 但无 scores/guess → 空结果（不 panic）
        let g = parse_guess_json("{\"wrong\": 1}").expect("合法 JSON 应解析");
        assert!(g.scores.is_empty());
        assert!(g.guess.is_empty());
        assert!(parse_guess_json("[]").is_none());
    }

    #[test]
    fn test_compress_screenshot() {
        // 生成一张小测试图 → 压缩 → 验证 base64 非空
        let dir = std::env::temp_dir().join(format!("dagent_vision_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("shot.png");
        let img = image::RgbImage::from_pixel(1600, 900, image::Rgb([120, 200, 80]));
        img.save(&path).expect("保存测试图失败");
        let b64 = compress_screenshot_to_b64(&path).expect("压缩失败");
        assert!(!b64.is_empty());
        // 应该变成 JPEG（base64 前几个字节解出来是 JPEG 头）
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&b64)
            .expect("base64 解码失败");
        assert_eq!(&bytes[0..3], &[0xFF, 0xD8, 0xFF], "应是 JPEG 格式");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 真实 API e2e（需要 QIANWEN_API_KEY，cargo test e2e_vision -- --ignored --nocapture）──

    #[tokio::test]
    #[ignore]
    async fn e2e_qwen_vision_understand() {
        let key = std::env::var("QIANWEN_API_KEY").unwrap_or_else(|_| {
            let cfg_path = r"D:\3Dagent\3dagent\config.json";
            let content = std::fs::read_to_string(cfg_path).unwrap_or_default();
            let content = content.trim_start_matches('\u{feff}');
            let v: Value = serde_json::from_str(content).unwrap_or(Value::Null);
            v.get("qianwen_api_key")
                .and_then(|k| k.as_str())
                .unwrap_or("")
                .to_string()
        });
        assert!(!key.is_empty(), "需要 QIANWEN_API_KEY");

        // 截图 → qwen3-vl-plus 描述
        let shot = std::env::temp_dir().join("dagent_e2e_shot.png");
        {
            let monitors = xcap::Monitor::all().expect("获取显示器失败");
            monitors[0]
                .capture_image()
                .expect("截屏失败")
                .save(&shot)
                .expect("保存失败");
        }
        let desc = describe_screen(&key, &shot).await.expect("识图失败");
        println!("=== 屏幕描述 ===\n{}", desc);
        assert!(!desc.is_empty());
        let _ = std::fs::remove_file(&shot);
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_activity_guess_real() {
        let key = std::env::var("QIANWEN_API_KEY").unwrap_or_else(|_| {
            let cfg_path = r"D:\3Dagent\3dagent\config.json";
            let content = std::fs::read_to_string(cfg_path).unwrap_or_default();
            let content = content.trim_start_matches('\u{feff}');
            let v: Value = serde_json::from_str(content).unwrap_or(Value::Null);
            v.get("qianwen_api_key")
                .and_then(|k| k.as_str())
                .unwrap_or("")
                .to_string()
        });
        assert!(!key.is_empty(), "需要 QIANWEN_API_KEY");

        let guess = call_activity_guess(
            &key,
            "窗口: VS Code | 空闲: 5s | 状态持续: 120s | 5min切换: 1次 | 状态: FocusedWork",
            "用户: 帮我看看这个 Rust 报错\nAI: 让我看看... 生命周期的问题",
            "focused_work",
        )
        .await
        .expect("activity_guess 调用失败")
        .expect("解析失败");
        println!("=== activity_guess ===\n{:?}", guess.guess);
        assert!(!guess.guess.is_empty());
    }
}
