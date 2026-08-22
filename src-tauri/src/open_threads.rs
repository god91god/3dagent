// ============================================================
// 未收尾话题检测（Open Threads）—— 语义版追问系统
// 借鉴自 N.E.K.O. main_logic/activity/llm_enrichment.py (call_open_threads)
// 源文件:
//   - N.E.K.O-main/main_logic/activity/llm_enrichment.py (call_open_threads)
//   - N.E.K.O-main/config/prompts/prompts_activity.py (OPEN_THREADS_PROMPTS)
// 原作者: Project N.E.K.O. Team (Copyright 2025-2026)
// 许可: Apache License 2.0 —— 本文件保留上游版权声明
//
// 功能：LLM 回顾最近对话，识别"被提起但还没收尾"的话题——AI 答应过但还没做的
// 事、用户说一半被打断没说完的话、双重需求只接住一边的遗憾等。检测结果注入
// 主动搭话 prompt，让优香在搭话时自然续上没聊完的话题（"你刚说那个 bug 后来呢？"）。
//
// 单用户简化版（相对上游改动）：
//   - 只做语义版（LLM 检测），不做规则版 unfinished_thread（问号启发式）
//   - 缓存：用户新消息 → 失效；后台重算（带冷却，防连续消息烧 LLM）
//   - 砍掉：open_threads 与 unfinished_thread 的抑制联动（我们没有规则版）
// ============================================================

use crate::memory_prompts::OPEN_THREADS_PROMPT;
use serde_json::Value;
use std::sync::Mutex;

// ── 常量 ──

/// 两次后台重算的最小间隔（秒）—— 用户连续发消息时不会每轮都调 LLM
pub const OPEN_THREADS_COOLDOWN_SECONDS: f64 = 60.0;
/// 最多检测出的未收尾话题数（抄 N.E.K.O. threads[:5]，我们取 3 更克制）
pub const OPEN_THREADS_MAX: usize = 3;

// ── 缓存 ──

/// 未收尾话题缓存（内存态即可，重启丢缓存可接受——本来就是"最近对话"的语义）
pub struct OpenThreadsCache {
    /// 已检测出的话题列表（空 = 没有未收尾话题）
    threads: Mutex<Vec<String>>,
    /// 上次成功计算时间戳（冷却用）
    last_computed_at: Mutex<f64>,
    /// 是否有新用户消息（用户发消息 → 缓存失效，需要重算）
    dirty: Mutex<bool>,
}

impl OpenThreadsCache {
    pub fn new() -> Self {
        OpenThreadsCache {
            threads: Mutex::new(Vec::new()),
            last_computed_at: Mutex::new(0.0),
            dirty: Mutex::new(false),
        }
    }

    /// 用户新消息 → 缓存失效（抄 N.E.K.O. tracker：cache invalidates on next user message）
    pub fn invalidate(&self) {
        *self.dirty.lock().unwrap() = true;
    }

    /// 当前是否有未收尾话题可注入（非 dirty 且非空）
    pub fn has_open_threads(&self) -> bool {
        if *self.dirty.lock().unwrap() {
            return false;
        }
        !self.threads.lock().unwrap().is_empty()
    }

    /// 取当前话题列表（非 dirty 时；dirty 时返回空）
    pub fn current(&self) -> Vec<String> {
        if *self.dirty.lock().unwrap() {
            return Vec::new();
        }
        self.threads.lock().unwrap().clone()
    }

    /// 写入新结果（清 dirty + 记时间）
    pub fn store(&self, threads: Vec<String>) {
        *self.threads.lock().unwrap() = threads;
        *self.last_computed_at.lock().unwrap() = now_secs();
        *self.dirty.lock().unwrap() = false;
    }

    /// 冷却是否已过（可以重算）
    pub fn cooldown_passed(&self) -> bool {
        now_secs() - *self.last_computed_at.lock().unwrap() >= OPEN_THREADS_COOLDOWN_SECONDS
    }

    /// 是否需要重算（用户发过消息且冷却已过）
    pub fn should_recompute(&self) -> bool {
        *self.dirty.lock().unwrap() && self.cooldown_passed()
    }
}

// ── LLM 检测（抄 N.E.K.O. call_open_threads）──

/// 调用 LLM 检测最近对话中的未收尾话题
/// 返回: Ok(Vec<String>) 最多 OPEN_THREADS_MAX 条；失败 → Err
pub async fn detect_open_threads(
    api_key: &str,
    conversation: &str,
) -> Result<Vec<String>, String> {
    let prompt = OPEN_THREADS_PROMPT.replace("{CONVERSATION}", conversation);

    let body = serde_json::json!({
        "model": crate::chat_model(),
        "messages": [
            {"role": "system", "content": "你是对话回顾助手，只输出 JSON。"},
            {"role": "user", "content": prompt}
        ],
        "stream": false,
        "max_tokens": 512,
        "response_format": {"type": "json_object"}
    });

    let client = crate::shared_http_client();
    let resp = client
        .post("https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions")
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("未收尾话题检测请求失败: {}", e))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("未收尾话题检测接口返回 {}: {}", status, text));
    }
    let resp_body: Value = resp
        .json()
        .await
        .map_err(|e| format!("未收尾话题检测响应解析失败: {}", e))?;
    let content = resp_body["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| "未收尾话题检测响应无内容".to_string())?;

    let parsed = parse_open_threads_json(content);
    // 清理：去空白 + 去重 + 截断
    let mut out: Vec<String> = Vec::new();
    for item in parsed {
        let text = item.trim().to_string();
        if text.is_empty() {
            continue;
        }
        if out.contains(&text) {
            continue;
        }
        out.push(text);
        if out.len() >= OPEN_THREADS_MAX {
            break;
        }
    }
    Ok(out)
}

/// 解析 open_threads JSON（兼容 {"open_threads": [...]} 包装 + markdown 围栏）
fn parse_open_threads_json(raw: &str) -> Vec<String> {
    let mut text = raw.trim().to_string();
    // 剥 markdown 围栏（```json ... ```）
    if let Some(start) = text.find("```") {
        let after = &text[start + 3..];
        let body_start = after.find('\n').map(|i| i + 1).unwrap_or(0);
        let body = &after[body_start..];
        if let Some(end) = body.rfind("```") {
            text = body[..end].trim().to_string();
        }
    }
    let v: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    match v.get("open_threads").and_then(|x| x.as_array()) {
        Some(arr) => arr
            .iter()
            .filter_map(|x| x.as_str().map(|s| s.to_string()))
            .collect(),
        None => Vec::new(),
    }
}

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ============================================================
// 单元测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_plain_json() {
        let raw = r#"{"open_threads": ["用户说想吃顿好的，没人接", "AI 答应明天整理笔记"]}"#;
        let threads = parse_open_threads_json(raw);
        assert_eq!(threads.len(), 2);
        assert_eq!(threads[0], "用户说想吃顿好的，没人接");
    }

    #[test]
    fn test_parse_fenced_json() {
        let raw = "```json\n{\"open_threads\": [\"话题A\"]}\n```";
        let threads = parse_open_threads_json(raw);
        assert_eq!(threads, vec!["话题A"]);
    }

    #[test]
    fn test_parse_empty() {
        assert_eq!(parse_open_threads_json(r#"{"open_threads": []}"#), Vec::<String>::new());
        // 非对象 / 缺字段 → 空
        assert_eq!(parse_open_threads_json("[]"), Vec::<String>::new());
        assert_eq!(parse_open_threads_json("not json"), Vec::<String>::new());
    }

    #[test]
    fn test_cache_invalidation_flow() {
        let cache = OpenThreadsCache::new();
        assert!(!cache.has_open_threads());
        // 存入话题 → 可注入
        cache.store(vec!["话题A".to_string()]);
        assert!(cache.has_open_threads());
        assert_eq!(cache.current(), vec!["话题A"]);
        // 用户新消息 → 失效
        cache.invalidate();
        assert!(!cache.has_open_threads());
        assert_eq!(cache.current(), Vec::<String>::new());
        // 冷却没过 → 不重算
        assert!(!cache.should_recompute());
    }

    #[test]
    fn test_cooldown_gate() {
        let cache = OpenThreadsCache::new();
        cache.store(vec![]);
        // 刚算过 → 冷却未过，不重算
        cache.invalidate();
        assert!(!cache.should_recompute());
        // 手动把 last_computed_at 调旧
        *cache.last_computed_at.lock().unwrap() = now_secs() - OPEN_THREADS_COOLDOWN_SECONDS - 1.0;
        assert!(cache.should_recompute());
    }

    #[test]
    fn test_detect_cleans_dedup_truncate() {
        // 只测纯函数部分：解析 + 清理逻辑由 detect_open_threads 的循环保证
        let raw = r#"{"open_threads": ["  ", "话题A", "话题A", "话题B", "话题C", "话题D"]}"#;
        let parsed = parse_open_threads_json(raw);
        // 解析阶段不过滤空白/去重，交给 detect 循环
        assert_eq!(parsed.len(), 6);
    }

    // ── 真实 API e2e（需要 QIANWEN_API_KEY，cargo test e2e_open_threads -- --ignored --nocapture）──

    #[tokio::test]
    #[ignore]
    async fn e2e_detect_open_threads() {
        let key = std::env::var("QIANWEN_API_KEY").unwrap_or_else(|_| {
            let cfg_path = r"D:\3Dagent\3dagent\config.json";
            let content = std::fs::read_to_string(cfg_path).unwrap_or_default();
            let content = content.trim_start_matches('\u{feff}');
            let v: serde_json::Value = serde_json::from_str(content).unwrap_or(serde_json::Value::Null);
            v.get("qianwen_api_key")
                .and_then(|k| k.as_str())
                .unwrap_or("")
                .to_string()
        });
        assert!(!key.is_empty(), "需要 QIANWEN_API_KEY 环境变量或 config.json");

        // 模拟真实对话：用户提到两件没说完的事
        let conversation = r#"用户: 今天好想吃顿好的犒劳一下自己
AI: 去呀！火锅还是烧烤？
用户: 但是我又想减肥，最近胖了
AI: 那可以吃清淡点的日料呀，蛋白质高还不胖
用户: 嗯……你说的对，改天试试
用户: 对了，之前说的那个 bug 我还没弄完，明天继续调
AI: 好的，别太累
用户: 我先去写作业了"#;

        let threads = detect_open_threads(&key, conversation)
            .await
            .expect("检测失败");
        println!("=== 未收尾话题 ===");
        for t in &threads {
            println!("- {}", t);
        }
        assert!(!threads.is_empty(), "应检测出至少 1 条未收尾话题");
        assert!(threads.len() <= OPEN_THREADS_MAX);
    }
}
