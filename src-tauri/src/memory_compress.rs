// ============================================================
// 近期对话压缩 —— 借鉴 N.E.K.O. memory/recent.py compress_history
// 源文件: N.E.K.O-main/memory/recent.py (CompressedRecentHistoryManager)
// 原作者: Project N.E.K.O. Team (Copyright 2025-2026)
// 许可: Apache License 2.0 —— 本文件保留上游版权声明
//
// 单用户简化版（相对上游改动）：
//   - 触发：recent.json 消息数 > 20 → 最旧 10 条压缩成摘要（append_recent 返回）
//   - 压缩：LLM 单次调用（RECENT_COMPRESS_PROMPT），失败静默跳过（保留原文下次再压）
//   - 合并：旧摘要 + 本轮压缩文本 → merge_recent_summary 落盘
//   - 砍掉：map-reduce 分段 / Stage-2 二次压缩 / 后台兜底任务 / 时间衰减 hint
//            （我们的对话量远小于 N.E.K.O. 多角色，单次调用足够）
// ============================================================

use crate::memory::MemoryStore;
use crate::memory_prompts::RECENT_COMPRESS_PROMPT;
use serde_json::Value;

/// 把旧消息渲染成压缩 prompt 输入文本（"主人/优香: 内容"）
fn render_messages_text(messages: &[Value]) -> String {
    let mut out = String::new();
    for m in messages {
        let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let content = m.get("content").and_then(|c| c.as_str()).unwrap_or("");
        if content.trim().is_empty() {
            continue;
        }
        let speaker = if role == "user" { "主人" } else { "优香" };
        out.push_str(&format!("{}: {}\n", speaker, content));
    }
    out
}

/// 调用 LLM 压缩一段旧对话 → 摘要文本
/// 返回 Ok(Some(text)) = 压缩摘要；Ok(None) = 输入为空；Err = 调用失败
pub async fn compress_recent_messages(
    api_key: &str,
    messages: &[Value],
) -> Result<Option<String>, String> {
    let conversation = render_messages_text(messages);
    if conversation.trim().is_empty() {
        return Ok(None);
    }
    let prompt = RECENT_COMPRESS_PROMPT.replace("{CONVERSATION}", &conversation);

    let body = serde_json::json!({
        "model": crate::chat_model(),
        "messages": [
            {"role": "system", "content": "你是对话归档助手，只输出备忘录文本。"},
            {"role": "user", "content": prompt}
        ],
        "stream": false,
        "max_tokens": 512
    });

    let client = crate::shared_http_client();
    let resp = client
        .post("https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions")
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("对话压缩请求失败: {}", e))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("对话压缩接口返回 {}: {}", status, text));
    }
    let resp_body: Value = resp
        .json()
        .await
        .map_err(|e| format!("对话压缩响应解析失败: {}", e))?;
    let content = resp_body["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| "对话压缩响应无内容".to_string())?
        .trim()
        .to_string();

    if content.is_empty() || content.contains("[PASS]") {
        return Ok(None);
    }
    let clean = content
        .trim_matches(|c| c == '"' || c == '「' || c == '」' || c == '“' || c == '”')
        .trim()
        .to_string();
    if clean.is_empty() {
        return Ok(None);
    }
    Ok(Some(clean))
}

/// 一站式：压缩并合并进 recent.json（后台任务入口）
/// 失败静默返回 Err，由调用方记录日志（保留原文，下次超过阈值再压）
pub async fn compress_and_merge(
    store: &MemoryStore,
    api_key: &str,
    messages: &[Value],
) -> Result<Option<String>, String> {
    let summary = compress_recent_messages(api_key, messages).await?;
    if let Some(s) = &summary {
        store.merge_recent_summary(s)?;
    }
    Ok(summary)
}

// ============================================================
// 单元测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_render_messages_text() {
        let msgs = vec![
            json!({"role": "user", "content": "我叫小李，是大学生"}),
            json!({"role": "assistant", "content": "记住了！"}),
            json!({"role": "user", "content": "   "}), // 空白跳过
        ];
        let text = render_messages_text(&msgs);
        assert!(text.contains("主人: 我叫小李"));
        assert!(text.contains("优香: 记住了！"));
        assert!(!text.contains("空白跳过"));
    }

    #[test]
    fn test_empty_input() {
        let text = render_messages_text(&[]);
        assert!(text.is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_compress_recent() {
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

        // 模拟 10 条旧对话（含重要信息 + 寒暄）
        let mut msgs = Vec::new();
        for i in 0..5 {
            msgs.push(json!({"role": "user", "content": format!("今天想学 Rust 的第 {} 天", i)}));
            msgs.push(json!({"role": "assistant", "content": "加油呀主人！"}));
        }
        msgs.push(json!({"role": "user", "content": "对了，下周有个面试，帮我记着"}));
        msgs.push(json!({"role": "assistant", "content": "好的，记下了！"}));

        let summary = compress_recent_messages(&key, &msgs)
            .await
            .expect("压缩失败");
        println!("=== 压缩摘要 ===\n{:?}", summary);
        match summary {
            Some(s) => {
                assert!(!s.is_empty());
                assert!(s.chars().count() <= 300, "摘要应 ≤300 字: {}", s.chars().count());
            }
            None => println!("模型返回空（重试或换内容）"),
        }
    }

    #[tokio::test]
    #[ignore]
    async fn e2e_compress_and_merge_persist() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("dagent_compress_e2e_{}", n));
        let _ = std::fs::remove_dir_all(&dir);
        let store = MemoryStore::new(dir.clone());

        // 先塞满 20 条（10 轮 × 2）
        for i in 0..10 {
            store
                .append_recent("user", &format!("对话轮次 {} 的内容，主人提到喜欢喝咖啡", i))
                .unwrap();
            store
                .append_recent("assistant", &format!("优香回答第 {} 轮", i))
                .unwrap();
        }
        // 再塞 10 条（触发压缩，len 会从 20 增长，第 21 条起压缩到 10，之后累积到 19）
        for i in 10..20 {
            store
                .append_recent("user", &format!("对话轮次 {} 的内容，主人提到喜欢喝咖啡", i))
                .unwrap();
            store
                .append_recent("assistant", &format!("优香回答第 {} 轮", i))
                .unwrap();
        }
        // 此时 40 条：第 21 条压缩（留 10），继续累积，第 31 条再次压缩（留 10），
        // 之后 9 条 → 18 条（18 ≤ 20 不触发）→ 期望 18 条
        assert_eq!(store.get_recent().len(), 18, "压缩后最近消息数");
        // 再 append 3 条（19 → 20 不触发 → 21 触发压缩）：模拟真实超阈值
        let mut to_compress = None;
        for i in 0..3 {
            to_compress = store
                .append_recent("user", &format!("补充轮次 {}", i))
                .unwrap();
        }
        assert!(to_compress.is_some(), "第 21 条应返回待压缩消息");
        assert_eq!(store.get_recent().len(), 10, "压缩后保留最近 10 条");

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
        if !key.is_empty() {
            compress_and_merge(&store, &key, to_compress.as_deref().unwrap_or(&[]))
                .await
                .expect("压缩合并失败");
            let summary = store.get_recent_summary();
            println!("=== 持久化摘要 ===\n{}", summary);
            assert!(!summary.is_empty(), "摘要应写入 recent.json");
            // 重启模拟：新实例读到同一文件
            let store2 = MemoryStore::new(dir);
            assert_eq!(store2.get_recent_summary(), summary);
            assert_eq!(store2.get_recent().len(), 10);
        }
    }
}
