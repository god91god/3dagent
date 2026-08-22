// ============================================================
// post-turn 事实提取 —— 借鉴 N.E.K.O. facts.py Stage-1
// 每轮对话后：LLM 提取事实 → SHA-256 去重 → 落库
// 许可: Apache-2.0（同上游）
// ============================================================

use crate::memory::MemoryStore;
use crate::memory_prompts::FACT_EXTRACTION_PROMPT;
use serde_json::Value;

/// 调用 qwen 提取事实（长 prompt 后台任务：120s 长超时 + 重试 2 次）
pub async fn extract_facts(
    store: &MemoryStore,
    conversation: &str,
    api_key: &str,
    lanlan_name: &str,
    master_name: &str,
) -> Result<(usize, usize), String> {
    let prompt = FACT_EXTRACTION_PROMPT
        .replace("{LANLAN_NAME}", lanlan_name)
        .replace("{MASTER_NAME}", master_name)
        .replace("{CONVERSATION}", conversation);

    let body = serde_json::json!({
        "model": crate::chat_model(),
        "messages": [{
            "role": "user",
            "content": prompt
        }],
        "stream": false,
        "response_format": {"type": "json_object"}
    });

    // 长超时 Client：共享 60s 对长 prompt 不够（实测 6000 字 ~19s，抖动时更久）
    let client = crate::long_timeout_client()?;
    let mut last_err = String::new();
    for attempt in 0..3 {
        match client
            .post("https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions")
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
        {
            Ok(resp) => {
                if resp.status().is_success() {
                    let parsed_body: serde_json::Value = resp
                        .json()
                        .await
                        .map_err(|e| format!("提取事实响应解析失败: {}", e))?;
                    let content = parsed_body["choices"][0]["message"]["content"]
                        .as_str()
                        .ok_or_else(|| "提取事实响应无内容".to_string())?;
                    return parse_and_store(store, content);
                } else {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    last_err = format!("提取事实接口返回 {}: {}", status, text);
                }
            }
            Err(e) => {
                last_err = format!("提取事实请求失败: {}", e);
            }
        }
        // 抖动/限流 → 等 1s 重试
        if attempt < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        }
    }
    Err(last_err)
}

/// 解析 LLM 返回并落库（提取成功路径）
fn parse_and_store(store: &MemoryStore, content: &str) -> Result<(usize, usize), String> {
    // 解析：模型可能返回 markdown 包裹的 JSON（```json ... ```），或直接 JSON 数组，
    // 或 {"facts": [...]} 对象包装。注意：不能靠 from_str 失败来判断包装——
    // 对象 JSON 本身也是合法 JSON，from_str 会成功，必须解析后看类型
    let cleaned = strip_markdown_fence(content);
    let parsed: Value = serde_json::from_str(&cleaned)
        .map_err(|e| format!("提取结果 JSON 解析失败: {}（原始: {}）", e, content.chars().take(200).collect::<String>()))?;
    // 兼容 {"facts": [...]} 包装：取 facts 字段（若是对象）
    let parsed = if parsed.is_array() {
        parsed
    } else if let Some(arr) = parsed.get("facts") {
        arr.clone()
    } else {
        return Err(format!("提取结果不是数组且无 facts 字段（原始: {}）", content.chars().take(200).collect::<String>()));
    };

    let arr = parsed
        .as_array()
        .ok_or_else(|| "提取结果不是数组".to_string())?;

    // 收集 (text, importance, entity)
    let mut facts: Vec<(String, i32, String)> = Vec::new();
    for item in arr {
        let text = item.get("text").and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
        if text.is_empty() || text.len() < 4 {
            continue;
        }
        let importance = item.get("importance").and_then(|i| i.as_i64()).unwrap_or(5) as i32;
        let entity = item.get("entity").and_then(|e| e.as_str()).unwrap_or("master").to_string();
        // entity 归一化
        let entity = match entity.as_str() {
            "master" | "neko" | "relationship" => entity,
            _ => "master".to_string(),
        };
        facts.push((text, importance, entity));
    }

    // 落库（去重在 add_facts 内部）
    let (added, dup) = store.add_facts(&facts);
    Ok((added, dup))
}

/// 去掉 markdown 代码围栏（```json ... ```）
fn strip_markdown_fence(s: &str) -> String {
    let s = s.trim();
    if s.starts_with("```") {
        let mut lines = s.lines();
        lines.next(); // 跳过 ```
        let body: Vec<&str> = lines.collect();
        // 去掉结尾 ```
        let mut body = body;
        if let Some(last) = body.last() {
            if last.trim().starts_with("```") {
                body.pop();
            }
        }
        return body.join("\n");
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实 API 端到端测试（需要网络 + key），手动运行: cargo test e2e -- --ignored
    #[tokio::test]
    #[ignore]
    async fn e2e_extract_and_recall() {
        let dir = std::env::temp_dir().join("dagent_e2e_test");
        let _ = std::fs::remove_dir_all(&dir);
        let store = MemoryStore::new(dir.clone());

        // 从 config.json 读 key
        let cfg = std::fs::read_to_string(r"D:\3Dagent\3dagent\config.json").unwrap();
        let v: serde_json::Value = serde_json::from_str(&cfg).unwrap();
        let key = v["qianwen_api_key"].as_str().unwrap().to_string();

        let conversation = "主人: 我叫小王，是个大学生，学的是计算机专业\n优香: 哇，计算机专业好厉害！\n主人: 我最近在学 Rust，感觉挺有意思的\n优香: 加油！Rust 确实很棒\n主人: 对了，我最喜欢喝咖啡，一天要喝两杯";

        let (added, dup) = extract_facts(&store, conversation, &key, "优香", "主人")
            .await
            .expect("提取失败");
        println!("第一次提取: 新增 {} 条, 重复 {} 条", added, dup);
        assert!(added > 0, "应至少提取出 1 条事实");

        let facts = store.get_facts();
        for f in &facts {
            println!("[{}] imp={} entity={} {}", f.id, f.importance, f.entity, f.text);
        }

        // 二次提取同一对话 → 全部去重
        let (added2, dup2) = extract_facts(&store, conversation, &key, "优香", "主人")
            .await
            .expect("二次提取失败");
        println!("第二次提取: 新增 {} 条, 重复 {} 条", added2, dup2);
        assert_eq!(added2, 0, "二次提取应全部去重");

        // recall
        let recalled = store.recall("咖啡", 5);
        println!("回忆'咖啡': {}", recalled);
        assert!(recalled.contains("咖啡"), "应能回忆出咖啡");

        // 注入上下文
        let ctx = store.render_memory_context(8, 2000);
        println!("=== 记忆上下文注入 ===\n{}", ctx);
        assert!(ctx.contains("咖啡") || ctx.contains("Rust"), "注入应含记忆");
    }
}
